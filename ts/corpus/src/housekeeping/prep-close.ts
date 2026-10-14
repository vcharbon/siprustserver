/**
 * Closing a PREP whose cut is gone (a cut killed from outside, a process that
 * died before it finished its prep): the prep row gets its end, so the wipe
 * and every reader of `finished_at` stop treating it as in flight. Its
 * readings stand as recorded.
 *
 * The verb asks the evidence `sweep-close.ts` asks, on the prep's scale: no
 * live process answers for it (its job's pid, where it ran as one), and it
 * recorded no reading within {@link QUIET_MINUTES}. Either missing, it refuses
 * and closes nothing.
 */
import { Jobs, OpenPreps } from "@sip/store"
import * as Effect from "effect/Effect"
import { liveness } from "../jobs/job.js"
import { QUIET_MINUTES } from "./sweep-close.js"

/** What closing a prep came to: closed, or refused with why. */
export type Closed = { readonly _tag: "closed" } | { readonly _tag: "refused"; readonly why: string }

/** The prep closed, or why not; `now` is the clock its last reading is read against. */
export const close = Effect.fn("PrepClose.close")(function*(prepId: number, now: number = Date.now()) {
  const preps = yield* OpenPreps.Service
  const jobs = yield* Jobs.Service
  const prep = (yield* preps.all).find((one) => one.prepId === prepId)
  if (prep === undefined) return { _tag: "refused", why: `no open prep ${prepId}` } satisfies Closed
  const job = prep.jobId === null ? undefined : (yield* jobs.running).find((one) => one.id === prep.jobId)
  if (job !== undefined && (yield* liveness(job))._tag === "running") {
    return { _tag: "refused", why: `its job ${job.id} is alive (pid ${job.pid ?? "unknown"})` } satisfies Closed
  }
  if (prep.lastCutAt !== null && Date.parse(prep.lastCutAt) >= now - QUIET_MINUTES * 60_000) {
    return {
      _tag: "refused",
      why: `it recorded a reading at ${prep.lastCutAt}, within ${QUIET_MINUTES} min: something is still cutting`
    } satisfies Closed
  }
  yield* preps.close(prepId)
  return { _tag: "closed" } satisfies Closed
})
