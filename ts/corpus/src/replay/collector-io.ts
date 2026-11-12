/**
 * What a scope's collectors run with in this orchestration (`@sip/rig`
 * `CollectorRun`): their named box-wide locks, each the exclusive lock
 * `collector-<name>` under the lock root (`locks.ts`), taken all or none in
 * the order of their names; and where their events go, the run's output and,
 * on a job, the job's events, where a `lab-left-changed` event is the job's
 * mark (`@sip/store` `JobEvents`).
 */
import type { CollectorRun } from "@sip/rig"
import type { JobEvents } from "@sip/store"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import type * as FileSystem from "effect/FileSystem"
import type { Roots } from "../deployment/index.js"
import * as Locks from "../locks.js"

/** The prefix of a collector lock's name under the lock root, apart from the lanes' own. */
export const LOCK_PREFIX = "collector-"

/** The locker over the box-wide exclusive locks, a holder waiting up to `waitMs` for each. */
export const locker = (
  waitMs: number = Locks.DEFAULT_WAIT_MS
): Effect.Effect<CollectorRun.Locker, never, FileSystem.FileSystem | Roots.Service> =>
  Effect.map(Effect.context<FileSystem.FileSystem | Roots.Service>(), (context) => (names, holder) => {
    const taken: Array<Locks.Taken> = []
    const release = Effect.suspend(() => Effect.forEach(taken.splice(0).reverse(), (lock) => lock.release, { discard: true }))
    return Effect.gen(function*() {
      for (const name of [...new Set(names)].sort()) {
        const got = yield* Locks.exclusive(`${LOCK_PREFIX}${name}`, `collector ${holder}`, { waitMs }).pipe(
          Effect.mapError((error) => error.message)
        )
        if (!Locks.isTaken(got)) return yield* Effect.fail(got.held)
        taken.push(got)
      }
      const held = [...taken]
      return Effect.forEach(held.reverse(), (lock) => lock.release, { discard: true })
    }).pipe(
      Effect.onError(() => release),
      Effect.provideContext(context)
    )
  })

/** `event` as a line of the run's output. */
export const lineOf = (event: CollectorRun.Event): string =>
  `[collector] ${event.collector}: ${event.phase}${event.message === undefined ? "" : `: ${event.message}`}`

/** Where a scope's collector events go: the output, and the job `job` names where it is one; a store refusing one is said, never the run's failure. */
export const sinkOf = (
  job: { readonly id: number; readonly events: JobEvents.Interface } | undefined
): CollectorRun.Sink =>
(event) =>
  Effect.andThen(
    Console.log(lineOf(event)),
    job === undefined ? Effect.void : job.events.append(job.id, {
      subject: event.collector,
      phase: event.phase,
      clockMs: event.clock_ms,
      wallMs: event.wall_ms,
      message: event.message
    }).pipe(
      Effect.catchCause(() => Console.error(`[collector] job ${job.id}: the store refused the event ${event.phase} of ${event.collector}`))
    )
  )
