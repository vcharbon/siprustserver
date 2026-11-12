/**
 * The jobs page's reads and verbs: the queue paged
 * by status, one job with its progress, its request, its restart chain, its
 * verification lines and each of its batches with the lines its end gates answered,
 * its log by byte range, its gone and red cells (its attachments are `attachments.ts`'s) —
 * and STOP, RESTART and DELETE RESULTS, each what the pack's job command line does:
 * a row written and a signal sent, a new row launched detached, a delete
 * job over the runs the job opened. Everything served is a row or a file,
 * so the viewer's restart changes nothing the page shows.
 *
 * A verb the CLI refuses, this refuses with the same sentence: a stop of a
 * job that is not running, a restart of one still running or of a job with
 * no request, a delete while the job runs.
 */
import * as Delete from "@sip/corpus/housekeeping/delete-runs"
import * as Job from "@sip/corpus/jobs/job"
import * as JobLines from "@sip/corpus/jobs/job-lines"
import * as JobStart from "@sip/corpus/jobs/job-start"
import * as Request from "@sip/corpus/requests/request"
import { Batches, Jobs, Removal, Requests, Verifications } from "@sip/store"
import { Effect, Layer, Schema } from "effect"
import * as Context from "effect/Context"
import { lineViewOf, type VerificationLineView } from "./api.ts"
import { ViewerConfig } from "./config.ts"
import type { Host } from "./host.ts"
import { JobLog } from "./job-log.ts"
import { type Paths, pathsOf } from "./paths.ts"
import { SitePack } from "./site-pack.ts"

export interface ListQuery {
  readonly status: Jobs.StatusFilter | undefined
  readonly limit: number
  readonly offset: number
}

/** A job as the list shows it: the row's facts the page reads, and its counts. */
export interface Listed {
  readonly id: number
  readonly kind: Jobs.Kind
  readonly status: Jobs.Status
  readonly label: string
  readonly lane: string | null
  readonly requestId: number | null
  readonly createdAt: string
  readonly startedAt: string | null
  readonly finishedAt: string | null
  readonly exit: number | null
  readonly waitingOn: string | null
  readonly batchAt: number | null
  readonly batches: number | null
  readonly stopRequestedAt: string | null
  readonly restartOf: number | null
  /** `resume` | `again` for a restart; null for a first run. */
  readonly mode: "resume" | "again" | null
  readonly landed: number
  readonly total: number
  readonly failed: number
  readonly gone: number
}

export interface Page {
  readonly rows: ReadonlyArray<Listed>
  readonly total: number
  readonly limit: number
  readonly offset: number
}

/** The request a replay job runs, in the sizes and the sentence the page shows. */
export interface RequestSummary {
  readonly id: number
  readonly lane: string
  readonly cells: number
  readonly captures: number
  /** The selection as the request command's `create` would state it. */
  readonly selection: string
}

export interface Detail {
  readonly job: Jobs.Row
  readonly mode: "resume" | "again" | null
  readonly progress: Jobs.Progress
  readonly label: string
  readonly request: RequestSummary | null
  readonly lineage: Jobs.Lineage
  /** Where the job's log is, or null for a job that ran in the foreground. */
  readonly logPath: string | null
  /** The job's own verification lines. */
  readonly lines: ReadonlyArray<VerificationLineView>
  /** The job's batches in the order they started, each with the lines its end gates answered. */
  readonly batchList: ReadonlyArray<BatchView>
}

/** One batch of a job: its lane, its attempt, its cells, when it ran, what its end said, its lines. */
export interface BatchView {
  readonly id: number
  readonly lane: string
  readonly attempt: number
  readonly cells: number
  readonly startedAt: string
  /** Null while the batch has not ended. */
  readonly finishedAt: string | null
  /** What its end said of the batch; null on a clean end. */
  readonly detail: string | null
  readonly lines: ReadonlyArray<VerificationLineView>
}

export interface LogRange {
  readonly from: number
  readonly to: number
  readonly text: string
  /** The job has ended and the range reached the file's end: nothing more will come. */
  readonly done: boolean
}

export type CellsOutcome = "gone" | "red"

export type CellsSlice =
  | { readonly outcome: "gone"; readonly cells: ReadonlyArray<Jobs.GoneCell>; readonly total: number }
  | { readonly outcome: "red"; readonly cells: ReadonlyArray<Jobs.RedCell>; readonly total: number }

/** What deleting the job's runs takes: the runs, the rows, the bundles on disk, and what holds them. */
export interface DeletePlan {
  readonly runs: number
  readonly rows: Removal.DeleteRows
  readonly bundles: number
  readonly held: ReadonlyArray<Removal.HeldRun>
}

export interface Started {
  readonly id: number
  readonly kind: Jobs.Kind
  readonly label: string
  readonly logPath: string
}

export class Refused extends Schema.TaggedError<Refused>()("Site.JobConsoleRefused", {
  detail: Schema.String
}) {}

export class Missing extends Schema.TaggedError<Missing>()("Site.JobMissing", {
  id: Schema.Number
}) {}

export interface Interface {
  readonly list: (query: ListQuery) => Effect.Effect<Page>
  readonly show: (id: number) => Effect.Effect<Detail, Missing>
  /** The log from byte `from`: never the whole file. */
  readonly log: (id: number, from: number) => Effect.Effect<LogRange, Missing>
  readonly cells: (id: number, outcome: CellsOutcome, limit: number) => Effect.Effect<CellsSlice, Missing>
  /** The job command's `stop`: the stop on the row, then SIGTERM to the job's group. */
  readonly stop: (id: number, grace: number | undefined) => Effect.Effect<void, Missing | Refused>
  /** The job command's `restart --resume | --again`: a new job over the request, linked. */
  readonly restart: (id: number, mode: "resume" | "again") => Effect.Effect<Started, Missing | Refused>
  readonly deletePlan: (id: number) => Effect.Effect<DeletePlan, Missing>
  /** The job command's `start delete --of-job <id>`: the delete job, linked. */
  readonly delete: (id: number) => Effect.Effect<Started, Missing | Refused>
}

export class Service extends Context.Service<Service, Interface>()("@sip/site/JobConsole") {}

export const MAX_LIMIT = 500
export const DEFAULT_LIMIT = 50
export const DEFAULT_CELLS = 200

const ENDED: ReadonlySet<Jobs.Status> = new Set<Jobs.Status>(["done", "failed", "interrupted", "died"])

const modeOf = (row: Jobs.Row): Listed["mode"] =>
  row.restartOf === null ? null : row.args["unran"] === true ? "resume" : "again"

const listedOf = (row: Jobs.Listed): Listed => ({
  id: row.id,
  kind: row.kind,
  status: row.status,
  label: JobLines.what(row),
  lane: row.lane,
  requestId: row.requestId,
  createdAt: row.createdAt,
  startedAt: row.startedAt,
  finishedAt: row.finishedAt,
  exit: row.exit,
  waitingOn: row.waitingOn,
  batchAt: row.batchAt,
  batches: row.batches,
  stopRequestedAt: row.stopRequestedAt,
  restartOf: row.restartOf,
  mode: modeOf(row),
  landed: row.landed,
  total: row.total,
  failed: row.failed,
  gone: row.gone
})


export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const config = yield* ViewerConfig.Service
    const pack = yield* SitePack.Service
    const jobs = yield* Jobs.Service
    const requests = yield* Requests.Service
    const removal = yield* Removal.Service
    const batches = yield* Batches.Service
    const verifications = yield* Verifications.Service
    const host = yield* Effect.context<Host>()

    const triagePaths = (): Paths | Refused =>
      config.triage === undefined
        ? new Refused({ detail: `no triage block in ${config.configPath}: there is no store to act on a job in` })
        : pathsOf(pack, config.triage)

    const list = Effect.fn("JobConsole.list")(function*(query: ListQuery) {
      const limit = Math.min(MAX_LIMIT, Math.max(1, Math.floor(query.limit)))
      const offset = Math.max(0, Math.floor(query.offset))
      const page = yield* jobs.page({ status: query.status, limit, offset }).pipe(Effect.orDie)
      return { rows: page.rows.map(listedOf), total: page.total, limit, offset } satisfies Page
    })

    const detail = Effect.fn("JobConsole.detail")(function*(id: number) {
      const shown = yield* jobs.show(id).pipe(Effect.orDie)
      if (shown === null) return yield* new Missing({ id })
      return shown
    })

    const show = Effect.fn("JobConsole.show")(function*(id: number) {
      const shown = yield* detail(id)
      const row = shown.job
      const request = row.requestId === null ? null : yield* requests.get(row.requestId).pipe(Effect.orDie)
      const lineage = yield* jobs.lineage(id).pipe(Effect.orDie)
      const lines = yield* verifications.of({ scope: "job", jobId: id }).pipe(Effect.orDie)
      const ran = yield* batches.ofJob(id).pipe(Effect.orDie)
      const batchViews = yield* Effect.forEach(ran, (batch) =>
        verifications.of({ scope: "batch", batchId: batch.id }).pipe(
          Effect.orDie,
          Effect.map((held): BatchView => ({
            id: batch.id,
            lane: batch.lane,
            attempt: batch.attempt,
            cells: batch.cells.length,
            startedAt: batch.startedAt,
            finishedAt: batch.finishedAt,
            detail: batch.detail,
            lines: held.map(lineViewOf)
          }))
        ))
      return {
        job: row,
        mode: modeOf(row),
        progress: shown.progress,
        label: JobLines.what(row),
        request: request === null ? null : {
          id: request.id,
          lane: request.lane,
          cells: request.cells,
          captures: request.captures,
          selection: Request.describe(request.selection)
        },
        lineage,
        logPath: row.logPath,
        lines: lines.map(lineViewOf),
        batchList: batchViews
      } satisfies Detail
    })

    const log = Effect.fn("JobConsole.log")(function*(id: number, from: number) {
      const shown = yield* detail(id)
      const ended = ENDED.has(shown.job.status)
      if (shown.job.logPath === null) return { from: 0, to: 0, text: "", done: ended } satisfies LogRange
      // A log not yet written (the launcher's first instant) is empty, not an error.
      const range = yield* JobLog.readRange(shown.job.logPath, from).pipe(
        Effect.catch(() => Effect.succeed({ from, to: from, text: "", size: from }))
      )
      return { from: range.from, to: range.to, text: range.text, done: ended && range.to >= range.size } satisfies LogRange
    })

    const cells = Effect.fn("JobConsole.cells")(function*(id: number, outcome: CellsOutcome, limit: number) {
      yield* detail(id)
      const cap = Math.min(MAX_LIMIT, Math.max(1, Math.floor(limit)))
      if (outcome === "gone") {
        const slice = yield* jobs.gone(id, cap).pipe(Effect.orDie)
        return { outcome, cells: slice.cells, total: slice.total } satisfies CellsSlice
      }
      const slice = yield* jobs.red(id, cap).pipe(Effect.orDie)
      return { outcome, cells: slice.cells, total: slice.total } satisfies CellsSlice
    })

    const stop = Effect.fn("JobConsole.stop")(function*(id: number, grace: number | undefined) {
      const shown = yield* detail(id)
      if (shown.job.status !== "running") {
        return yield* new Refused({ detail: `job ${id} is ${shown.job.status}, not running` })
      }
      if ((yield* Job.liveness(shown.job).pipe(Effect.provide(host)))._tag === "ended") {
        return yield* new Refused({ detail: `job ${id}'s process is gone — \`${pack.jobCommand} reap\` closes it` })
      }
      const seconds = Math.max(0, Math.round(grace ?? Job.DEFAULT_GRACE))
      yield* jobs.stop(id, seconds).pipe(Effect.orDie)
      yield* Job.stop(shown.job).pipe(
        Effect.provide(host),
        Effect.mapError((error) => new Refused({ detail: error.message }))
      )
    })

    /** A launch that made no process is a refusal saying why. */
    const launched = <A, E>(effect: Effect.Effect<A, E, Jobs.Service | Host>): Effect.Effect<A, Refused> =>
      effect.pipe(
        Effect.catch((error) =>
          JobStart.isNotStarted(error) ? Effect.fail(new Refused({ detail: error.message })) : Effect.die(error)
        ),
        Effect.provideService(Jobs.Service, jobs),
        Effect.provide(host)
      )

    const restart = Effect.fn("JobConsole.restart")(function*(id: number, mode: "resume" | "again") {
      const paths = triagePaths()
      if (paths instanceof Refused) return yield* paths
      const shown = yield* detail(id)
      const old = shown.job
      if (old.kind !== "replay" || old.requestId === null) {
        return yield* new Refused({ detail: `job ${id} is a ${old.kind} job with no request to run again` })
      }
      if (!ENDED.has(old.status)) return yield* new Refused({ detail: `job ${id} is ${old.status} — stop it first` })
      const request = yield* requests.get(old.requestId).pipe(Effect.orDie)
      if (request === null) return yield* new Refused({ detail: `request ${old.requestId} of job ${id} is gone` })
      const knobs = { ...JobStart.replayKnobsOf(old.args), unran: mode === "resume" }
      const started = yield* launched(JobStart.replay(paths.sessions, request, knobs, old.id))
      return {
        id: started.id,
        kind: "replay",
        label: `replay of request ${request.id} on ${request.lane} (${mode} of job ${id})`,
        logPath: started.launched.logPath
      } satisfies Started
    })

    const deletePlan = Effect.fn("JobConsole.deletePlan")(function*(id: number) {
      yield* detail(id)
      const paths = triagePaths()
      if (paths instanceof Refused) return { runs: 0, rows: { runs: 0, caseResults: 0, runRuleHits: 0 }, bundles: 0, held: [] }
      const plan = yield* Delete.plan({ paths, selector: { _tag: "job", jobId: id }, measure: false }).pipe(
        Effect.provideService(Removal.Service, removal),
        Effect.provide(host),
        Effect.orDie
      )
      return { runs: plan.runs.length, rows: plan.rows, bundles: plan.bundles.length, held: plan.held } satisfies DeletePlan
    })

    const del = Effect.fn("JobConsole.delete")(function*(id: number) {
      const paths = triagePaths()
      if (paths instanceof Refused) return yield* paths
      const shown = yield* detail(id)
      if (!ENDED.has(shown.job.status)) return yield* new Refused({ detail: `job ${id} is ${shown.job.status} — stop it first` })
      const started = yield* launched(JobStart.deleteRuns(paths.sessions, { selector: { _tag: "job", jobId: id }, measure: false }))
      return {
        id: started.id,
        kind: "delete",
        label: `delete of the runs of job ${id}`,
        logPath: started.launched.logPath
      } satisfies Started
    })

    return Service.of({ list, show, log, cells, stop, restart, deletePlan, delete: del })
  })
)

export * as JobConsole from "./jobs.ts"
