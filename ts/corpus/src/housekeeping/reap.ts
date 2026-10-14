/**
 * Closing run rows nobody is coming back for.
 *
 * `runs.finished_at IS NULL` means "in flight", and every reader believes it:
 * the retention pass reads the cut store from the lowest prep such a row pins,
 * so an open run keeps every cut log and every cache entry its own reading can
 * name. A killed sweep leaves its rows open forever, so that one crash makes the
 * passes behind this one permanently more conservative than the tree deserves.
 *
 * A row is declared dead only on TWO pieces of evidence at once, one from each
 * store:
 *
 * - the database says nobody will close it — its sweep has already ended, or it
 *   is older than {@link HORIZON_HOURS} with no sweep to speak for it;
 * - the disk agrees — nothing has been written into its bundle within the same
 *   horizon, the bundle having never been created counting as nothing written.
 *
 * Neither half alone is enough. A run's bundle does not exist for as long as
 * its batch is running, so age alone would reap a live sweep's rows; and a
 * bundle's mtime says nothing about whether a process still holds the row.
 *
 * The ROW survives, as every row does. Reaping stamps `finished_at` and says
 * why in `detail`; a run that never reported keeps the `error` status it was
 * opened with, because that is what it is.
 *
 * A JOB is reaped on one piece of evidence, because a job has a process: a
 * `running` row whose process is gone ended without closing itself — `failed`
 * or `done` with the exit its supervisor recorded, `died` where none was — and
 * the runs it opened and left open are closed with it, whatever their
 * bundles' age: nobody is coming back for them.
 */
import { Jobs, Runs } from "@sip/store"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Option from "effect/Option"
import * as path from "node:path"
import { liveness } from "../jobs/job.js"

/**
 * How long a row must have gone unclosed and its bundle unwritten. Several
 * times the longest batch a full-corpus sweep runs, so no sweep can outlive it
 * mid-flight; short enough that one killed sweep does not hold the tree
 * conservative for a week.
 */
export const HORIZON_HOURS = 12

export const DETAIL = "abandoned: no process closed this run, and its bundle stopped changing"

export const JOB_DETAIL = "abandoned: the job that opened this run is gone"

/** A path's mtime, Unix milliseconds; 0 where it has none to read. */
const mtimeOf = (fs: FileSystem.FileSystem, file: string) =>
  fs.stat(file).pipe(
    Effect.map((info) => Option.match(info.mtime, { onNone: () => 0, onSome: (date) => date.getTime() })),
    Effect.orElseSucceed(() => 0)
  )

/**
 * The newest thing written into a bundle, one level deep. A cell writes
 * `<bundle>/<cellDir>/…` and `<bundle>/specs/…`, and either bumps the mtime of
 * the bundle or of the child it writes under, so depth one answers "is anything
 * still writing here" without a recursive walk. A bundle that does not exist
 * has never been written to, which is `0`.
 */
export const lastWrite = Effect.fn("Reap.lastWrite")(function*(bundle: string) {
  const fs = yield* FileSystem.FileSystem
  let newest = yield* mtimeOf(fs, bundle)
  if (newest === 0) return 0
  const entries = yield* fs.readDirectory(bundle).pipe(Effect.orElseSucceed(() => [] as Array<string>))
  for (const entry of entries) newest = Math.max(newest, yield* mtimeOf(fs, path.join(bundle, entry)))
  return newest
})

export interface Reaped {
  /** Rows the database offered as candidates. */
  readonly examined: number
  /**
   * The rows the disk agreed were dead. They are closed unless this was a dry
   * run, and either way the passes BEHIND this one must read them as closed:
   * a preview that still counted them open would under-report what it drops.
   */
  readonly dead: ReadonlyArray<number>
}

export interface ReapRequest {
  readonly horizonHours: number
  /** A dry run rules on every row and closes none. */
  readonly dryRun: boolean
}

export const reap = Effect.fn("Reap.reap")(function*(request: ReapRequest) {
  const runs = yield* Runs.Service
  const candidates = yield* runs.abandonable(request.horizonHours)
  const before = Date.now() - request.horizonHours * 3_600_000
  const dead: Array<number> = []
  for (const row of candidates) if ((yield* lastWrite(row.bundlePath)) < before) dead.push(row.id)
  if (!request.dryRun) yield* runs.abandon(dead, DETAIL)
  return { examined: candidates.length, dead } satisfies Reaped
})

/** What one reaping pass is worth saying, or nothing where it found nothing. */
export const line = (reaped: Reaped, dryRun: boolean): string | undefined =>
  reaped.examined === 0
    ? undefined
    : `[cleanup] ${reaped.examined} unfinished row(s) nobody will close; ` +
      `${dryRun ? "would close" : "closed"} ${reaped.dead.length} whose bundle also went quiet`

/** What the job pass found: the jobs ruled ended, each with its end and the runs closed under it. */
export interface ReapedJob {
  readonly id: number
  readonly close: Jobs.CloseRequest
  readonly runs: ReadonlyArray<number>
}

/**
 * Every `running` job whose process is gone, closed as its record says, its
 * open runs closed with it. A dry run rules and closes nothing.
 */
export const reapJobs = Effect.fn("Reap.reapJobs")(function*(dryRun: boolean) {
  const jobs = yield* Jobs.Service
  const runs = yield* Runs.Service
  const reaped: Array<ReapedJob> = []
  for (const row of yield* jobs.running) {
    const state = yield* liveness(row)
    if (state._tag === "running") continue
    const open = yield* runs.unfinishedOf([row.id])
    reaped.push({ id: row.id, close: state.close, runs: open })
    if (dryRun) continue
    yield* runs.abandon(open, JOB_DETAIL)
    yield* jobs.close(row.id, state.close)
  }
  return reaped as ReadonlyArray<ReapedJob>
})

/** One line per job the pass ruled on. */
export const jobLines = (reaped: ReadonlyArray<ReapedJob>, dryRun: boolean): ReadonlyArray<string> =>
  reaped.map((job) =>
    `[cleanup] job ${job.id} ${dryRun ? "would read" : "reads"} ${job.close.status}` +
    (job.close.exit === null ? "" : ` (exit ${job.close.exit})`) +
    `: its process is gone; ${job.runs.length} open run(s) closed`
  )
