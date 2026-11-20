/**
 * What a pack's run verb is made of beside its phases: a line refused with
 * exit 2 (a job's row closed `failed` first, its log naming the line), a run's
 * directory stamp, the load a lane's run row states, the run closed on every
 * lane however it ends before its report, and the exit code of a run held by
 * another one or crashed. Every line on stderr starts with the verb.
 */
import { Jobs } from "@sip/store"
import type { LaneView } from "@sip/rig"
import * as Cause from "effect/Cause"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as Predicate from "effect/Predicate"
import type { RunHeld } from "./exclusive.js"
import { pacingOf } from "./lanes.js"
import * as Record from "./record.js"
import { CELL_MEMORY_MAX } from "./replay.js"

/** `reason` on stderr as the `verb` line's refusal, the process's exit code 2. */
export const usage = (verb: string, reason: string) =>
  Effect.flatMap(Console.error(`${verb}: ${reason}`), () =>
    Effect.sync(() => {
      process.exitCode = 2
    }))

/** The refusal of a `verb` line launched as the job `job`: its log names the line first, and its row is closed `failed` with exit 2. */
export const refuserOf = (verb: string, job: number | undefined) => (reason: string) =>
  job === undefined
    ? usage(verb, reason)
    : Effect.andThen(
      Console.error(`${verb}: command: ${process.argv.slice(1).join(" ")}`),
      Effect.andThen(
        usage(verb, reason),
        Effect.ignore(Effect.flatMap(Jobs.Service, (jobs) => jobs.close(job, { status: "failed", exit: 2 })))
      )
    )

/** A run directory's stamp: the time now, filesystem-safe. */
export const stamp = (): string => new Date().toISOString().replace(/[:.]/g, "-")

/** The load the run row of `found` states: its pacing, one cell's memory box. */
export const loadOf = (found: LaneView.View) => {
  const pacing = pacingOf(found)
  return {
    concurrency: pacing.concurrency,
    batchSize: 0,
    startsPerSecond: pacing.startIntervalMs === undefined ? 0 : 1000 / pacing.startIntervalMs,
    maxSpanSeconds: null,
    memoryMax: CELL_MEMORY_MAX
  }
}

/** `run` with `record` closed by `store` where it ends before its report (a crash, an interrupt): every lane not yet closed an error. */
export const closedOnExit = <A, E, R>(
  store: Pick<Record.RunStore, "finish">,
  record: Record.Record,
  run: Effect.Effect<A, E, R>
) =>
  run.pipe(Effect.onExit((exit) =>
    exit._tag === "Success"
      ? Effect.void
      : Record.abort(store, record, `the run ended before its report: ${Cause.pretty(exit.cause).split("\n")[0]}`).pipe(Effect.ignore)
  ))

/** Whether `error` is another run holding the environment, by its tag: the same failure however its class was loaded. */
const isHeld = <E>(error: E): error is E & RunHeld => Predicate.isTagged(error, "CampaignRunner.RunHeld")

/** The exit code of the `verb` run `program`: its own, 2 where another run holds the environment or it crashed with no call judged. */
export const exitCodeOf = <E, R>(verb: string, program: Effect.Effect<0 | 1 | 2, E, R>) =>
  program.pipe(
    Effect.catchIf(isHeld, (held) => Effect.as(Console.error(`${verb}: ${held.message}`), 2 as const)),
    Effect.catchCause((cause) => Effect.as(Console.error(`${verb}: crashed: ${Cause.pretty(cause)}`), 2 as const))
  )
