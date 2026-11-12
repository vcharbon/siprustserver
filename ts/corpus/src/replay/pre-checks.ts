/**
 * A job's pre-checks (`@sip/rig` `PreCheck`, ADR-0060 appendix §3), whoever
 * runs the job (a capture replay, a campaign): the job-scope collectors `at =
 * "before"` the deployment wires over the job's lanes, run before the job's
 * provisioning and its first call. What they answer of the job are its lines
 * and attachments, after any it holds; their events are the job's. A `fail`
 * refuses the job with its message, as does a required pre-check, of either
 * scope, that a real-clock lane of the job names no part of.
 *
 * A batch's own pre-checks run in the batch driver (`lane-batch.ts`).
 */
import type { Attachment, Verification } from "@sip/contracts"
import { PreCheck } from "@sip/rig"
import { Attachments, JobEvents, Verifications } from "@sip/store"
import * as Clock from "effect/Clock"
import * as Effect from "effect/Effect"
import * as path from "node:path"
import { Collectors } from "../deployment/index.js"
import * as Locks from "../locks.js"
import * as CollectedCells from "./collected-cells.js"
import * as CollectorIo from "./collector-io.js"

/** What a job's pre-checks run for. */
export interface Request {
  /** The job's row, where it is one: its events, lines and attachments are its own. */
  readonly job: number | undefined
  /** The job's directory: its pre-checks' directories under `collectors/`, its own attachments. */
  readonly dir: string
  readonly lanes: ReadonlyArray<string>
  /** Told whom a pre-check's lock waits for, and `undefined` once it is taken. */
  readonly watch?: Locks.Watch
}

/** `lines` written after those job `job` holds; nothing where none is given. */
export const appendJobLines = (job: number, lines: ReadonlyArray<Verification.Verification>) =>
  lines.length === 0 ? Effect.void : Effect.flatMap(Verifications.Service, (store) =>
    Effect.flatMap(store.of({ scope: "job", jobId: job }), (held) => store.writeJob(job, [...held, ...lines])))

/** `answers` as job `job`'s lines and attachments, `dir` holding the attached files; a store refusing them is said, never the job's failure. */
const answerJob = (
  job: number,
  dir: string,
  answers: { readonly verifications: ReadonlyArray<Verification.Verification>; readonly attachments: ReadonlyArray<Attachment.Attachment> }
) =>
  Effect.gen(function*() {
    yield* appendJobLines(job, answers.verifications)
    if (answers.attachments.length > 0) {
      yield* Effect.flatMap(Attachments.Service, (store) => store.add({ scope: "job", jobId: job }, { dir, attachments: answers.attachments }))
    }
  }).pipe(Effect.catchCause(() => Effect.logError(`job ${job}: the store refused its pre-checks' answers`)))

/** Runs the job's pre-checks: why the job is refused, or `undefined` where it is not or none is wired. */
export const job = (request: Request) =>
  Effect.gen(function*() {
    const seam = yield* Collectors.Service
    const instances = (yield* seam.job(request.lanes, path.join(request.dir, "collectors"), request.dir)).filter(PreCheck.isPreCheck)
    const unwired = yield* seam.unwired(request.lanes)
    if (instances.length === 0 && unwired.length === 0) return undefined
    const events = yield* JobEvents.Service
    const checked = yield* PreCheck.run(instances, unwired, {
      sink: CollectorIo.sinkOf(request.job === undefined ? undefined : { id: request.job, events }),
      locker: yield* CollectorIo.locker(Locks.DEFAULT_WAIT_MS, request.watch)
    })
    if (request.job !== undefined) yield* answerJob(request.job, request.dir, checked.answers)
    return checked.refusal
  })

/**
 * {@link job} for work no job row files (a sweep, a campaign's or an evidence
 * run's replay): its directory under `sessions`' collectors, its events the
 * output's only.
 */
export const unfiled = (sessions: string, lanes: ReadonlyArray<string>) =>
  Effect.flatMap(Clock.currentTimeMillis, (ms) =>
    job({ job: undefined, dir: path.join(sessions, CollectedCells.COLLECTORS_DIR, `pre-checks-${ms}-${process.pid}`), lanes }))
