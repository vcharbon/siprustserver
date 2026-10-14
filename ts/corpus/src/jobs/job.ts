/**
 * A job's PROCESS: launched detached (`@sip/toolchain` `Detached`), so the
 * launcher's own death — a shell, a viewer — kills nothing, and answered for
 * afterwards from what the launch records.
 *
 * The record lives under `<sessions>/_jobs/<id>/`: `cmd`, `log` (stdout and
 * stderr, whole), `pid` (the supervisor's pid with its start ticks), `exit`
 * (written by the supervisor the instant the command ends), and `hooks/` for
 * the hook logs. The row names the log; everything else is beside it. A STOP
 * is a TERM to the supervisor's process group — the supervisor leads its own
 * session, so the group is the job's whole tree; a foreground job, which has
 * no supervisor, is TERMed by pid.
 *
 * LIVENESS is read from the record and the kernel, never guessed: a `running`
 * row whose process is gone ended without closing its row — with the exit the
 * supervisor wrote, or, when that is absent too, `died`.
 */
import type { Jobs } from "@sip/store"
import { Detached, Liveness } from "@sip/toolchain"
import * as Effect from "effect/Effect"
import type * as FileSystem from "effect/FileSystem"
import * as Schema from "effect/Schema"
import * as path from "node:path"

/** Where a session tree keeps its job records. */
export const JOBS = "_jobs"

export const jobsDir = (sessions: string): string => path.join(sessions, JOBS)

export const jobDir = (sessions: string, id: number): string => path.join(jobsDir(sessions), String(id))

export const hooksPath = (sessions: string, id: number): string => path.join(jobDir(sessions, id), "hooks")

/** What the launcher knows once the process exists. */
export interface Launched {
  readonly pid: number
  readonly logPath: string
  readonly hooksPath: string
}

/** Why a launch made no process. */
export class NotLaunched extends Schema.TaggedError<NotLaunched>()("Job.NotLaunched", { detail: Schema.String }) {
  override get message(): string {
    return this.detail
  }
}

/**
 * The command, detached in `cwd`: the supervisor's pid is the job's, and its
 * record is the job's directory. It returns with the log in place and the
 * supervisor leading its group, so a stop at once reaches the job. Refused
 * where the id's record still runs, which no fresh id ever does.
 */
export const launch = (
  sessions: string,
  id: number,
  argv: ReadonlyArray<string>,
  cwd: string
): Effect.Effect<Launched, NotLaunched, Detached.Requirements> =>
  Detached.launch(jobDir(sessions, id), argv, { cwd }).pipe(
    Effect.map((launched) => ({
      pid: launched.pid,
      logPath: launched.log,
      hooksPath: hooksPath(sessions, id)
    } satisfies Launched)),
    Effect.mapError((error) => new NotLaunched({ detail: `job ${id} could not be launched: ${error.detail}` }))
  )

/** The seconds a stop gives the cells in flight where it states none. */
export const DEFAULT_GRACE = 30

/** Why a stop reached no process. */
export class NotStopped extends Schema.TaggedError<NotStopped>()("Job.NotStopped", { detail: Schema.String }) {
  override get message(): string {
    return this.detail
  }
}

/** Whether the row's process runs: by its record where it has one, by its pid alone otherwise. */
export const processAlive = Effect.fn("Job.processAlive")(function*(row: Jobs.Row) {
  if (row.pid === null) return false
  if (row.logPath !== null) {
    const record = yield* Liveness.recorded(path.join(path.dirname(row.logPath), "pid"))
    if (record !== undefined && record.pid === row.pid) return yield* Liveness.recordedAlive(record)
  }
  return yield* Liveness.alive(row.pid)
})

/**
 * SIGTERM to the job's process: its supervisor's whole group where the job
 * runs detached, its own pid where it runs in the foreground. Refused where
 * the row names no live process — the liveness rule is what closes that row.
 */
export const stop = (row: Jobs.Row): Effect.Effect<void, NotStopped, FileSystem.FileSystem> =>
  Effect.gen(function*() {
    if (row.pid === null || !(yield* processAlive(row))) {
      return yield* new NotStopped({ detail: `job ${row.id} has no live process to stop` })
    }
    yield* Detached.terminate(row.pid, row.logPath !== null).pipe(
      Effect.mapError((error) => new NotStopped({ detail: error.detail }))
    )
  })

/** The exit the supervisor recorded beside the log, or undefined where it wrote none. */
export const exitOf = (logPathOfJob: string): Effect.Effect<number | undefined, never, FileSystem.FileSystem> =>
  Detached.exitOf(path.dirname(logPathOfJob))

/**
 * How a `running` row stands: still running, or how it ended without saying
 * so. An exit the supervisor recorded is the process's own; a process gone
 * with none recorded — a killed group, a rebooted box, a foreground run that
 * was killed — `died`.
 */
export type Liveness = { readonly _tag: "running" } | { readonly _tag: "ended"; readonly close: Jobs.CloseRequest }

export const liveness = Effect.fn("Job.liveness")(function*(row: Jobs.Row) {
  if (yield* processAlive(row)) return { _tag: "running" } as Liveness
  const exit = row.logPath === null ? undefined : yield* exitOf(row.logPath)
  if (exit === undefined) return { _tag: "ended", close: { status: "died", exit: null } } as Liveness
  return { _tag: "ended", close: { status: exit === 0 ? "done" : "failed", exit } } as Liveness
})
