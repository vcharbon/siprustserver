/**
 * A cut run in the FOREGROUND as a `cut` job: the row made `running` under
 * this process's pid, the captures its plan, the prep opened under it
 * (`preps.job_id`), each capture's line as its batch lands, the row closed
 * with the cut's own verdict — `failed` where a capture broke or its rows
 * could not be written, `failed` before a defect propagates. So a cut a
 * campaign takes in passing is listed among the jobs exactly as a launched
 * one is.
 */
import { Cuts, Jobs } from "@sip/store"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as Fingerprint from "../fingerprint.js"
import * as JobStart from "../jobs/job-start.js"
import type { Options } from "../options.js"
import * as Provenance from "../provenance.js"
import { censusGate, type Cut, phase } from "./cut.js"

/** What the job came to: the prep it cut under, the row it ran as, and the cut itself. */
export interface Done {
  readonly prep: number
  readonly job: number
  readonly cut: Cut
}

/** The phase the job drives: the real one, or a stand-in under test. */
export type Phase = typeof phase

const failed = (cut: Cut): boolean =>
  cut.mirrored.failed > 0 || [...cut.readings.values()].some((reading) => reading.broke !== undefined)

/**
 * The captures cut under one prep of a job of this process's own; `by` names
 * the caller on the opening line. The row's `args` are what a launched
 * stale-only cut over the same list records.
 */
export const run = Effect.fn("CutJob.run")(function*(
  captures: ReadonlyArray<string>,
  options: Options,
  by: string,
  cut: Phase = phase,
  gate: typeof censusGate = censusGate
) {
  // The gate of the whole cut, once, before its job and its prep open.
  yield* gate(captures, options)
  const jobs = yield* Jobs.Service
  const cuts = yield* Cuts.Service
  const args: JobStart.CutRequest = {
    captures,
    standing: false,
    all: false,
    known: false,
    mode: "stale-only",
    slice: undefined,
    concurrency: options.concurrency,
    memoryMax: options.memoryMax
  }
  const job = yield* JobStart.runningAs(undefined, "cut", { ...args }, options.paths.sessions).pipe(
    Effect.catchIf(JobStart.isNoSuchJob, (refused) => Effect.die(refused))
  )
  yield* jobs.planned(job, captures.length)
  const fingerprint = yield* Fingerprint.cut
  const prep = yield* cuts.open(yield* Provenance.current, fingerprint, job)
  yield* Console.log(
    `[${by}] cut: ${captures.length} capture(s) under prep${prep} as job ${job} ` +
      `(code ${Fingerprint.short(fingerprint)})`
  )
  const done = yield* cut(captures, options, prep, job).pipe(
    Effect.tapCause(() => jobs.close(job, { status: "failed", exit: 1 }).pipe(Effect.ignore))
  )
  yield* cuts.finish(prep)
  yield* jobs.close(job, failed(done) ? { status: "failed", exit: 1 } : { status: "done", exit: 0 })
  return { prep, job, cut: done } satisfies Done
})
