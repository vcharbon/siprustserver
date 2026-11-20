/**
 * The campaign job row a run answers for: the one it was launched as
 * (`--job`, a campaign job), else its own, made once the run holds its
 * environment's run lock and running under the run's process; a quick run
 * (`quick.ts`) is no job. The run closes it however it ends —
 * `done` when every cell was judged (pass or fail), `failed` when the run
 * errored or its job holds the mark of a lab left changed (`@sip/store`
 * `JobEvents`), a stopped run's included, `interrupted` (exit 130) when it
 * was stopped. Its row counts the
 * cells the run planned and those judged so far, and its batches are filed
 * under it.
 */
import { JobEvents, Jobs } from "@sip/store"
import * as Cause from "effect/Cause"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import type { ExitCode } from "./run.js"

/** Why `id` is no row a run can answer for; undefined where it is a campaign job. */
export const refusalOf = (id: number) =>
  Effect.map(Jobs.Service.pipe(Effect.flatMap((jobs) => jobs.show(id))), (row): string | undefined =>
    row === null
      ? `there is no job ${id}`
      : row.job.kind !== "campaign"
      ? `job ${id} is a ${row.job.kind} job, not a campaign job`
      : undefined)

/** The counts a campaign job's row shows: its planned cells, the judged ones (`settled`), neither failing the run. */
export const progressOf = (jobs: Jobs.Interface, id: number) => ({
  planned: (cells: number) => Effect.ignore(jobs.planned(id, cells)),
  judged: (cells: number) => Effect.ignore(jobs.settle(id, cells))
})

/** How a run that ended `exit` closes its job row, `labLeftChanged` where the job holds that mark. */
export const closeOf = (exit: Exit.Exit<ExitCode, unknown>, labLeftChanged: boolean): Jobs.CloseRequest =>
  labLeftChanged
    ? { status: "failed", exit: 2 }
    : Exit.isSuccess(exit)
    ? exit.value === 2 ? { status: "failed", exit: 2 } : { status: "done", exit: exit.value }
    : Cause.hasInterruptsOnly(exit.cause)
    ? { status: "interrupted", exit: 130 }
    : { status: "failed", exit: 2 }

/** What a campaign job's row states of its run, as a launched one's states it (`@sip/corpus` `JobStart.campaign`). */
export interface Launch {
  readonly capture: string
  readonly lanes: ReadonlyArray<string>
  readonly case: string | null
  readonly subcase: string | null
  readonly rebuild: boolean
  readonly allow: ReadonlyArray<string>
}

/** The run's own campaign job row, `running` under this process. */
export const ownOf = (launch: Launch, createdBy: string) =>
  Effect.gen(function*() {
    const jobs = yield* Jobs.Service
    const id = yield* jobs.create({ kind: "campaign", args: { ...launch }, createdBy })
    yield* jobs.started(id, { pid: process.pid })
    return id
  })

/** Whether the job `id` holds the mark of a lab left changed; a store that cannot say is said on a `verb` line, and the job unmarked. */
const markedOf = (verb: string, id: number) =>
  Effect.flatMap(JobEvents.Service, (events) => events.labLeftChanged(id)).pipe(
    Effect.catchCause((cause) =>
      Effect.as(Console.error(`${verb}: job ${id}: whether the lab was left changed not read: ${Cause.pretty(cause)}`), false)
    )
  )

/**
 * `body`, the job row `idOf` names once it ends closed however it ends, its
 * mark read then; none where it names none; a close the store refuses is
 * said on a `verb` line, never the run's failure.
 */
const closing = <E, R>(verb: string, idOf: () => number | undefined, body: Effect.Effect<ExitCode, E, R>) =>
  body.pipe(
    Effect.onExit((exit) => {
      const id = idOf()
      return id === undefined ? Effect.void : Effect.flatMap(
        markedOf(verb, id),
        (marked) => Effect.flatMap(Jobs.Service, (jobs) => jobs.close(id, closeOf(exit, marked)))
      ).pipe(
        Effect.catchCause((cause) => Console.error(`${verb}: job ${id} not closed: ${Cause.pretty(cause)}`))
      )
    })
  )

/**
 * `body` run as the run's job, under the run lock `hold`, in a scope of its
 * own: the job it was launched as (`launched`), closed however the run ends,
 * a refused lock included; else, where `own` states the run, a row of its own
 * made only once `hold` holds, so a refused run leaves none, and closed
 * likewise; else (a quick run) no job. A row closes once the scope released
 * the lock and every resource of `body`. What it cannot close is said on a
 * line starting with the pack's `verb`.
 */
export const asJob = <E, R, EH, RH>(
  verb: string,
  launched: number | undefined,
  own: { readonly launch: Launch; readonly createdBy: string } | undefined,
  hold: Effect.Effect<void, EH, RH>,
  body: (job: number | undefined) => Effect.Effect<ExitCode, E, R>
) =>
  Effect.suspend(() => {
    let made: number | undefined = launched
    const jobOf = own === undefined || launched !== undefined
      ? Effect.succeed(launched)
      : Effect.tap(ownOf(own.launch, own.createdBy), (id) => Effect.sync(() => void (made = id)))
    return closing(
      verb,
      () => made,
      Effect.scoped(Effect.andThen(hold, Effect.flatMap(jobOf, body)))
    )
  })
