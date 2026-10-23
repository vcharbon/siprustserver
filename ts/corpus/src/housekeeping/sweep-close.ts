/**
 * Closing a SWEEP whose process is gone (a campaign stopped from outside, a
 * kill): its open runs land `error` with the reaper's own detail, its job —
 * where it ran as one — closes as its record says, and the sweep row gets its
 * end, so the passes that read `finished_at` stop treating it as in flight.
 *
 * The verb asks for the same two pieces of evidence the reaper does
 * (`reap.ts`), on the sweep's scale: no live process answers for it (its job's
 * pid, where it has a job), and none of its open runs' bundles has been
 * written within {@link QUIET_MINUTES}. Either missing, it refuses and closes
 * nothing.
 */
import { Jobs, Runs, Sweeps } from "@sip/store"
import * as Effect from "effect/Effect"
import { liveness } from "../jobs/job.js"
import { DETAIL, lastWrite } from "./reap.js"

/** How long every open bundle of the sweep must have gone unwritten. */
export const QUIET_MINUTES = 10

/** What closing a sweep came to: closed, or refused with why. */
export type Closed =
  | { readonly _tag: "closed"; readonly runs: ReadonlyArray<number>; readonly job: number | null }
  | { readonly _tag: "refused"; readonly why: string }

/** The sweep closed, or why not; `now` is the clock the bundles' quiet is read against. */
export const close = Effect.fn("SweepClose.close")(function*(sweepId: number, now: number = Date.now()) {
  const sweeps = yield* Sweeps.Service
  const runs = yield* Runs.Service
  const jobs = yield* Jobs.Service
  const row = (yield* sweeps.list(Number.MAX_SAFE_INTEGER)).find((one) => one.id === sweepId)
  if (row === undefined) return { _tag: "refused", why: `no sweep ${sweepId}` } satisfies Closed
  if (row.finishedAt !== null) {
    return { _tag: "refused", why: `sweep ${sweepId} already ended at ${row.finishedAt}` } satisfies Closed
  }
  const job = (yield* jobs.running).find((one) => one.sweepId === sweepId)
  const state = job === undefined ? undefined : yield* liveness(job)
  if (job !== undefined && state?._tag === "running") {
    return { _tag: "refused", why: `its job ${job.id} is alive (pid ${job.pid ?? "unknown"})` } satisfies Closed
  }
  const open = yield* runs.openOfSweep(sweepId)
  const quietSince = now - QUIET_MINUTES * 60_000
  const busy: Array<(typeof open)[number]> = []
  for (const run of open) if ((yield* lastWrite(run.bundlePath)) >= quietSince) busy.push(run)
  if (busy.length > 0) {
    return {
      _tag: "refused",
      why: `${busy.length} of its open run(s) wrote their bundle within ${QUIET_MINUTES} min ` +
        `(run${busy[0]!.id}): something is still writing`
    } satisfies Closed
  }
  const ids = open.map((run) => run.id)
  yield* runs.abandon(ids, DETAIL)
  if (job !== undefined && state?._tag === "ended") yield* jobs.close(job.id, state.close)
  yield* sweeps.finish(sweepId)
  return { _tag: "closed", runs: ids, job: job?.id ?? null } satisfies Closed
})
