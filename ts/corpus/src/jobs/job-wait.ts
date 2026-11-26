/**
 * Waiting on one job row until it ends: the process's own close, or the
 * liveness rule applied here when the process is gone without one
 * (`housekeeping/reap.ts`). What `job wait` runs, and any command that
 * launches a job and is asked to see it through.
 */
import { Jobs } from "@sip/store"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as Reap from "../housekeeping/reap.js"
import * as Job from "./job.js"
import { showLines } from "./job-lines.js"

/** How often the row is read while it runs. */
const POLL_MS = 2_000

/** The statuses a row ends in. */
export const ENDED: ReadonlySet<Jobs.Status> = new Set<Jobs.Status>(["done", "failed", "interrupted", "died"])

/** The exit code of a wait that gave up with the job still running. */
export const TIMED_OUT = 124

/**
 * Blocks until job `id` ends, then prints its row and answers the job's exit
 * code; 2 where there is no such job, {@link TIMED_OUT} once `timeoutSeconds`
 * pass with it still running (0 waits for ever).
 */
export const untilEnded = Effect.fn("JobWait.untilEnded")(function*(id: number, timeoutSeconds: number) {
  const jobs = yield* Jobs.Service
  const deadline = timeoutSeconds > 0 ? Date.now() + timeoutSeconds * 1000 : undefined
  for (;;) {
    const detail = yield* jobs.show(id)
    if (detail === null) {
      yield* Console.error(`[job] refused: no job ${id}`)
      return 2
    }
    if (ENDED.has(detail.job.status)) {
      for (const line of showLines(detail)) yield* Console.log(line)
      return detail.job.exit ?? (detail.job.status === "done" ? 0 : 1)
    }
    if (detail.job.status === "running" && (yield* Job.liveness(detail.job))._tag === "ended") {
      for (const line of Reap.jobLines(yield* Reap.reapJobs(false), false)) yield* Console.log(line)
      continue
    }
    if (deadline !== undefined && Date.now() >= deadline) {
      yield* Console.error(`[job] job ${id} still ${detail.job.status} after ${timeoutSeconds} s`)
      return TIMED_OUT
    }
    yield* Effect.sleep(POLL_MS)
  }
})
