/**
 * The setup phase of a run (upstream ADR-0060 appendix §3, §7): its named
 * campaign's setup operations injected after provisioning, around the rest of
 * the run, and healed in reverse once its report is written, however the run
 * ends (`@sip/rig` `Setup`). Each phase of an operation is a line of the run's
 * output and, on a campaign job, an event of the job (`@sip/store`
 * `JobEvents`): a heal failing twice marks the job, makes each lane its report
 * closed an error and makes the run exit 2.
 */
import type { JobEvents } from "@sip/store"
import { OperationRun, Setup } from "@sip/rig"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import { PhaseFailed } from "./run.js"

/** `event` as the run's output line, opening with `[<tag>]`: `setup`, or `step` for a case's step. */
export const lineOf = (event: OperationRun.Event, tag = "setup"): string =>
  `[${tag}] ${event.operation}: ${event.phase}${event.message === undefined ? "" : `: ${event.message}`}`

/** Where the run's operation events go: its output (its lines tagged `tag`), and the job `job` names where it is one; a store that refuses an event is said, never the run's failure. */
export const sinkOf = (
  job: { readonly id: number; readonly events: JobEvents.Interface } | undefined,
  tag = "setup"
): OperationRun.Sink =>
(event) =>
  Effect.andThen(
    Console.log(lineOf(event, tag)),
    job === undefined ? Effect.void : job.events.append(job.id, {
      subject: event.operation,
      phase: event.phase,
      clockMs: event.clock_ms,
      wallMs: event.wall_ms,
      message: event.message
    }).pipe(Effect.catchCause(() => Console.error(`[${tag}] job ${job.id}: the store refused the event ${event.phase} of ${event.operation}`)))
  )

/**
 * The setup phase over `instances`, its events to `sink`: `body` run inside
 * it, a failed inject the phase `setup`'s failure. Once every heal ran,
 * however the run ended, `leftChanged` is told the operations that left the
 * lab changed, where any did.
 */
export const setupOf = <RL>(
  instances: ReadonlyArray<OperationRun.Instance>,
  sink: OperationRun.Sink,
  leftChanged: (operations: ReadonlyArray<string>) => Effect.Effect<void, never, RL>
) =>
<A, R>(body: Effect.Effect<A, never, R>): Effect.Effect<Setup.Healed<A>, PhaseFailed, R | RL> =>
  Effect.suspend(() => {
    const watched = Setup.watching(sink)
    return Setup.around(instances, watched.sink, body).pipe(
      Effect.onExit(() => {
        const left = watched.leftChanged()
        return left.length === 0 ? Effect.void : leftChanged(left)
      }),
      Effect.mapError((failed) => new PhaseFailed({ phase: "setup", reason: failed.message }))
    )
  })

/** What a run's record says of the setup operations `operations` that left the lab changed. */
export const leftChangedOf = (operations: ReadonlyArray<string>): string =>
  `setup: lab left changed by ${operations.join(", ")}: ${OperationRun.HEAL_ATTEMPTS} heal attempts failed`
